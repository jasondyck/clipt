//! Recording through the ScreenCast portal (org.freedesktop.portal.ScreenCast):
//! for the Flatpak, and for desktops with neither a wlroots capture protocol
//! nor a KWin or Mutter screencast. The portal asks the user which monitor or
//! window to share; the answer is remembered with a restore token, so the
//! dialog appears once. `capture::pipewire` reads the stream it opens.

use super::LogicalRect;
use super::pipewire::{CropFn, Remote};
use crate::capture::VideoFrame;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use zbus::{OwnedFd as ZFd, OwnedVariant as OwnedValue};
use zbus::{OwnedObjectPath, Variant as Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.ScreenCast";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Monitor = 1,
    Window = 2,
}

pub fn available(conn: &crate::dbus::Connection) -> bool {
    crate::dbus::Proxy::new(conn, DEST, PATH, IFACE).and_then(|p| p.get_property::<u32>("version")).is_ok()
}

/// What the portal shares: its node, and where it sits on the desktop
/// (logical position and size; absent for windows).
#[derive(Debug, PartialEq)]
pub struct Shared {
    pub node: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// Record what the user picks (`kind`) until `sink` returns false. With
/// `region` (desktop-logical), frames are cropped to it inside the picked
/// monitor.
pub fn stream(kind: Kind, region: Option<LogicalRect>, cursor: bool, sink: Box<dyn FnMut(VideoFrame) -> bool>) -> Result<()> {
    let conn = crate::dbus::session().context("connecting to the session bus")?;
    let portal = crate::dbus::Proxy::new(&conn, DEST, PATH, IFACE)?;
    let mut n = 0;
    let mut request = |method: &str, call: &dyn Fn(&str) -> zbus::Result<OwnedObjectPath>| -> Result<HashMap<String, OwnedValue>> {
        n += 1;
        let token = format!("clipt{}_{n}", std::process::id());
        let sender = conn.unique_name().context("no unique bus name")?.trim_start_matches(':').replace('.', "_");
        let path = format!("{PATH}/request/{sender}/{token}");
        // Subscribe before calling, or a fast answer can arrive before we listen.
        let req = crate::dbus::Proxy::new(&conn, DEST, path.as_str(), "org.freedesktop.portal.Request")?;
        let mut responses = req.receive_signal("Response")?;
        call(&token).with_context(|| format!("ScreenCast {method}"))?;
        let msg = responses.next().with_context(|| format!("the portal closed without answering {method}"))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => bail!("screen sharing was cancelled in the portal dialog"),
            _ => bail!("the portal refused {method} (response {code})"),
        }
    };

    let created = request("CreateSession", &|t| {
        let mut o: HashMap<&str, Value> = HashMap::new();
        o.insert("handle_token", Value::from(t));
        o.insert("session_handle_token", Value::from(t));
        portal.call("CreateSession", &(o,))
    })?;
    let session: String = created.get("session_handle").context("no session from the portal")?.try_clone()?.try_into()?;
    let session = zbus::ObjectPath::try_from(session.as_str())?.into_owned();

    let modes: u32 = portal.get_property("AvailableCursorModes").unwrap_or(1);
    let token = load_token();
    request("SelectSources", &|t| {
        let mut o: HashMap<&str, Value> = HashMap::new();
        o.insert("handle_token", Value::from(t));
        o.insert("types", Value::from(kind as u32));
        o.insert("multiple", Value::from(false));
        // cursor_mode: 1 hidden, 2 embedded
        o.insert("cursor_mode", Value::from(if cursor && modes & 2 != 0 { 2u32 } else { 1u32 }));
        // Remember the choice until it's revoked, so the dialog appears once.
        o.insert("persist_mode", Value::from(2u32));
        if let Some(tk) = &token {
            o.insert("restore_token", Value::from(tk.as_str()));
        }
        portal.call("SelectSources", &(&session, o))
    })?;
    let started = request("Start", &|t| {
        let mut o: HashMap<&str, Value> = HashMap::new();
        o.insert("handle_token", Value::from(t));
        portal.call("Start", &(&session, "", o))
    })?;
    if let Some(tk) = started.get("restore_token").and_then(|v| String::try_from(v.try_clone().ok()?).ok()) {
        save_token(&tk);
    }
    let shared = parse_streams(started.get("streams").context("the portal shared nothing")?)?;
    let fd: ZFd = portal.call("OpenPipeWireRemote", &(&session, HashMap::<&str, Value>::new())).context("OpenPipeWireRemote")?;
    let fd: OwnedFd = fd.into();
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: portal screencast node {} at {:?} size {:?}", shared.node, shared.position, shared.size);
    }
    let crop: CropFn = match (region, shared.position, shared.size) {
        (Some(r), Some(pos), Some(size)) => Box::new(move |w, h| Some(crop_in(r, pos, size, (w, h)))),
        _ => Box::new(|_, _| None),
    };
    let r = super::pipewire::stream(Remote { fd: Some(fd), node: shared.node }, crop, sink);
    let close = crate::dbus::Proxy::new(&conn, DEST, session.as_str(), "org.freedesktop.portal.Session");
    let _ = close.and_then(|s| s.call::<_, ()>("Close", &()));
    r
}

/// The first shared stream from Start's `streams` result: `a(ua{sv})`.
fn parse_streams(v: &OwnedValue) -> Result<Shared> {
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = v.try_clone()?.try_into().context("unexpected streams from the portal")?;
    let (node, props) = streams.into_iter().next().context("the portal shared no stream")?;
    let pair = |k: &str| props.get(k).and_then(|v| <(i32, i32)>::try_from(v.try_clone().ok()?).ok());
    Ok(Shared { node, position: pair("position"), size: pair("size") })
}

/// `region` (desktop-logical) as a crop in buffer pixels of a stream showing
/// the monitor at `pos` with logical `size`, delivered as `frame` pixels.
fn crop_in(region: LogicalRect, pos: (i32, i32), size: (i32, i32), frame: (u32, u32)) -> (u32, u32, u32, u32) {
    let (fw, fh) = frame;
    let scale = fw as f64 / size.0.max(1) as f64;
    let (x, y, w, h) = region;
    let px = |v: f64| (v * scale).round().max(0.0) as u32;
    let (cx, cy) = (px(x - pos.0 as f64).min(fw - 1), px(y - pos.1 as f64).min(fh - 1));
    (cx, cy, px(w).clamp(1, fw - cx), px(h).clamp(1, fh - cy))
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| crate::config::home().join(".local/state"));
    base.join("clipt/screencast-restore-token")
}

fn load_token() -> Option<String> {
    std::fs::read_to_string(token_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn save_token(token: &str) {
    let p = token_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, token);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_a_region_inside_the_shared_monitor() {
        // A 1280x720 monitor at x=1280, delivered at 2x.
        assert_eq!(crop_in((1380.0, 50.0, 301.0, 200.0), (1280, 0), (1280, 720), (2560, 1440)), (200, 100, 602, 400));
        // Never past the frame.
        assert_eq!(crop_in((2500.0, 700.0, 500.0, 500.0), (1280, 0), (1280, 720), (1280, 720)), (1220, 700, 60, 20));
    }

    #[test]
    fn reads_the_first_stream() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert("position".into(), OwnedValue::try_from(Value::from((1280i32, 0i32))).unwrap());
        props.insert("size".into(), OwnedValue::try_from(Value::from((1920i32, 1080i32))).unwrap());
        let v = OwnedValue::try_from(Value::from(vec![(57u32, props)])).unwrap();
        assert_eq!(parse_streams(&v).unwrap(), Shared { node: 57, position: Some((1280, 0)), size: Some((1920, 1080)) });
    }
}
