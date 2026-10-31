//! xdg-desktop-portal Screenshot: the lowest common denominator. Works on GNOME
//! (which exposes nothing else to unprivileged clients) and anywhere a portal
//! backend is installed. Returns one image of the whole desktop, which we slice
//! back into per-output shots.

use super::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

pub fn available(conn: &zbus::blocking::Connection) -> bool {
    zbus::blocking::Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.Screenshot")
        .and_then(|p| p.get_property::<u32>("version"))
        .is_ok()
}

pub fn capture(conn: &zbus::blocking::Connection, outputs: &[Output]) -> Result<Vec<Shot>> {
    let token = format!("glint{}", std::process::id());
    let sender = conn.unique_name().context("no unique bus name")?.trim_start_matches(':').replace('.', "_");
    let req_path = format!("{PATH}/request/{sender}/{token}");

    // Subscribe before calling, or a fast backend can answer before we listen.
    let req = zbus::blocking::Proxy::new(conn, DEST, req_path.as_str(), "org.freedesktop.portal.Request")?;
    let mut responses = req.receive_signal("Response")?;

    let shot = zbus::blocking::Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.Screenshot")?;
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(token.as_str()));
    opts.insert("interactive", Value::from(false));
    opts.insert("modal", Value::from(false));
    let _handle: zbus::zvariant::OwnedObjectPath = shot.call("Screenshot", &("", opts))?;

    let msg = responses.next().context("portal closed without answering")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    match code {
        0 => {}
        1 => bail!("screenshot was cancelled or denied in the portal dialog"),
        _ => bail!("portal screenshot failed (response {code})"),
    }
    let uri: String = results.get("uri").context("portal returned no uri")?.try_clone()?.try_into()?;
    let path = uri.strip_prefix("file://").context("non-file uri from portal")?;
    let path = percent_decode(path);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    // The portal drops a file in ~/Pictures; it's ours to clean up.
    let _ = std::fs::remove_file(&path);
    let full = Image::decode_png(&bytes)?;
    Ok(split(&full, outputs))
}

/// Slice a whole-desktop image into per-output images using logical geometry.
pub fn split(full: &Image, outputs: &[Output]) -> Vec<Shot> {
    let minx = outputs.iter().map(|o| o.x).min().unwrap_or(0);
    let miny = outputs.iter().map(|o| o.y).min().unwrap_or(0);
    let maxx = outputs.iter().map(|o| o.x + o.width as i32).max().unwrap_or(full.width as i32);
    let scale = full.width as f64 / (maxx - minx).max(1) as f64;
    outputs
        .iter()
        .map(|o| {
            let px = |v: i32| (v as f64 * scale).round() as u32;
            let image = full.crop(px(o.x - minx), px(o.y - miny), px(o.width as i32), px(o.height as i32));
            Shot { output: o.clone(), image }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
