//! KDE Plasma: org.kde.KWin.ScreenShot2. Fast and silent, but KWin only answers
//! processes whose executable is listed in a .desktop file carrying
//! `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`.

use super::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use zbus::zvariant::{Fd, OwnedValue, Value};

const DEST: &str = "org.kde.KWin.ScreenShot2";
const PATH: &str = "/org/kde/KWin/ScreenShot2";

pub fn available(conn: &zbus::blocking::Connection) -> bool {
    zbus::blocking::fdo::DBusProxy::new(conn)
        .ok()
        .and_then(|p| p.name_has_owner(DEST.try_into().unwrap()).ok())
        .unwrap_or(false)
}

pub fn capture(conn: &zbus::blocking::Connection, outputs: &[Output], cursor: bool) -> Result<Vec<Shot>> {
    let proxy = zbus::blocking::Proxy::new(conn, DEST, PATH, DEST)?;
    let mut shots = Vec::new();
    for o in outputs {
        let image = capture_screen(&proxy, &o.name, cursor).with_context(|| format!("KWin CaptureScreen({})", o.name))?;
        shots.push(Shot { output: o.clone(), image });
    }
    Ok(shots)
}

fn capture_screen(proxy: &zbus::blocking::Proxy, name: &str, cursor: bool) -> Result<Image> {
    let (rx, tx): (OwnedFd, OwnedFd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::from(rx);
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    });
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("include-cursor", Value::from(cursor));
    opts.insert("native-resolution", Value::from(true));
    let reply: HashMap<String, OwnedValue> = {
        let r = proxy.call("CaptureScreen", &(name, opts, Fd::from(&tx)));
        drop(tx); // our copy must close or the reader never sees EOF
        r?
    };
    let data = reader.join().map_err(|_| anyhow::anyhow!("pipe reader panicked"))??;

    let get = |k: &str| -> Result<u32> {
        reply.get(k).and_then(|v| u32::try_from(v).ok()).with_context(|| format!("missing '{k}' in reply"))
    };
    if let Some(t) = reply.get("type") {
        let t: &str = t.downcast_ref().unwrap_or("?");
        if t != "raw" {
            bail!("unsupported image type '{t}'");
        }
    }
    let (w, h, stride, format) = (get("width")?, get("height")?, get("stride")? as usize, get("format")?);
    if std::env::var_os("GLINT_DEBUG").is_some() {
        eprintln!("kwin: {w}x{h} stride {stride} format {format} reply {reply:?}");
    }
    decode_qimage(w, h, stride, format, &data)
}

/// Decode the QImage::Format values KWin is known to emit.
fn decode_qimage(w: u32, h: u32, stride: usize, format: u32, d: &[u8]) -> Result<Image> {
    match format {
        // RGB32 / ARGB32 / ARGB32_Premultiplied: 0xAARRGGBB native-endian = BGRA bytes
        4 => Image::from_rows(w, h, stride, 4, d, |p| [p[2], p[1], p[0], 255]),
        5 | 6 => Image::from_rows(w, h, stride, 4, d, |p| unpremul([p[2], p[1], p[0], p[3]], format == 6)),
        // RGBX8888 / RGBA8888 / RGBA8888_Premultiplied: byte-ordered RGBA
        16 => Image::from_rows(w, h, stride, 4, d, |p| [p[0], p[1], p[2], 255]),
        17 | 18 => Image::from_rows(w, h, stride, 4, d, |p| unpremul([p[0], p[1], p[2], p[3]], format == 18)),
        // RGBX64 / RGBA64 / RGBA64_Premultiplied: u16 per channel
        25..=27 => Image::from_rows(w, h, stride, 8, d, |p| {
            let c = |i: usize| (u16::from_ne_bytes([p[i], p[i + 1]]) >> 8) as u8;
            let a = if format == 25 { 255 } else { c(6) };
            unpremul([c(0), c(2), c(4), a], format == 27)
        }),
        // RGBX/RGBA16FPx4 (half floats), as used for HDR outputs
        30..=32 => Image::from_rows(w, h, stride, 8, d, |p| {
            let c = |i: usize| f16_to_u8(u16::from_ne_bytes([p[i], p[i + 1]]));
            let a = if format == 30 { 255 } else { c(6) };
            unpremul([c(0), c(2), c(4), a], format == 32)
        }),
        _ => bail!("unsupported QImage format {format}"),
    }
}

fn unpremul(p: [u8; 4], premul: bool) -> [u8; 4] {
    if !premul || p[3] == 255 || p[3] == 0 {
        return p;
    }
    let a = p[3] as u32;
    let f = |c: u8| ((c as u32 * 255 + a / 2) / a).min(255) as u8;
    [f(p[0]), f(p[1]), f(p[2]), p[3]]
}

fn f16_to_u8(h: u16) -> u8 {
    let sign = h >> 15;
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    let v = match exp {
        0 => man / 1024.0 * 2f32.powi(-14),
        31 => 1.0,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 {
        return 0;
    }
    // Values are linear-light; encode with sRGB transfer for an 8-bit PNG.
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5) as u8
}
