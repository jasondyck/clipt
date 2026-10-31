//! xdg-desktop-portal Screenshot: the lowest common denominator. Works on GNOME
//! (which exposes nothing else to unprivileged clients) and anywhere a portal
//! backend is installed. Returns one image of the whole desktop, which we slice
//! back into per-output shots.

use super::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use zbus::{OwnedVariant as OwnedValue, Variant as Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// Unsandboxed apps have no app ID unless they register one, and GNOME then
/// re-asks screenshot permission every time; registering as "clipt" lets
/// "Allow" stick. Must precede any other portal call on this connection and
/// needs xdg-desktop-portal 1.19+, so failure is ignored.
pub fn register(conn: &crate::dbus::Connection) {
    let _ = crate::dbus::Proxy::new(conn, DEST, PATH, "org.freedesktop.host.portal.Registry")
        .and_then(|p| p.call::<_, ()>("Register", &("clipt", HashMap::<&str, Value>::new())));
}

pub fn available(conn: &crate::dbus::Connection) -> bool {
    crate::dbus::Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.Screenshot").and_then(|p| p.get_property::<u32>("version")).is_ok()
}

/// `all` is every output (the portal image covers the whole desktop);
/// `targets` are the ones to return.
pub fn capture(conn: &crate::dbus::Connection, all: &[Output], targets: &[Output]) -> Result<Vec<Shot>> {
    let token = format!("clipt{}", std::process::id());
    let sender = conn.unique_name().context("no unique bus name")?.trim_start_matches(':').replace('.', "_");
    let req_path = format!("{PATH}/request/{sender}/{token}");

    // Subscribe before calling, or a fast backend can answer before we listen.
    let req = crate::dbus::Proxy::new(conn, DEST, req_path.as_str(), "org.freedesktop.portal.Request")?;
    let mut responses = req.receive_signal("Response")?;

    let shot = crate::dbus::Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.Screenshot")?;
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(token.as_str()));
    opts.insert("interactive", Value::from(false));
    opts.insert("modal", Value::from(false));
    let _handle: zbus::OwnedObjectPath = shot.call("Screenshot", &("", opts))?;

    let msg = responses.next().context("portal closed without answering")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    match code {
        0 => {}
        1 => bail!("screenshot was cancelled or denied in the portal dialog"),
        _ => bail!(
            "the portal refused the screenshot (response {code}); GNOME asks permission for \
             non-interactive screenshots — see README (Install → GNOME)"
        ),
    }
    let uri: String = results.get("uri").context("portal returned no uri")?.try_clone()?.try_into()?;
    let path = uri.strip_prefix("file://").context("non-file uri from portal")?;
    let path = percent_decode(path);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    // The portal drops a file in ~/Pictures; it's ours to clean up.
    let _ = std::fs::remove_file(&path);
    let full = Image::decode_png(&bytes)?;
    Ok(split(&full, all, targets))
}

/// Slice a whole-desktop image into per-output images. The portal renders the
/// whole desktop at one scale (GNOME uses the highest monitor scale), so the
/// scale comes from the full layout, and each slice is then resampled to its
/// monitor's native resolution: a 1x monitor next to a 2x one yields its own
/// 1280x720, not a 2560x1440 upscale.
pub fn split(full: &Image, all: &[Output], targets: &[Output]) -> Vec<Shot> {
    let minx = all.iter().map(|o| o.x).min().unwrap_or(0);
    let miny = all.iter().map(|o| o.y).min().unwrap_or(0);
    let maxx = all.iter().map(|o| o.x + o.width as i32).max().unwrap_or(full.width as i32);
    let scale = full.width as f64 / (maxx - minx).max(1) as f64;
    targets
        .iter()
        .map(|o| {
            let px = |v: i32| (v as f64 * scale).round() as u32;
            let mut image = full.crop(px(o.x - minx), px(o.y - miny), px(o.width as i32), px(o.height as i32));
            let (nw, nh) = o.native;
            if nw > 0 && nh > 0 && (nw, nh) != (image.width, image.height) {
                image = image.resize(nw, nh);
            }
            Shot { output: o.clone(), image }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
