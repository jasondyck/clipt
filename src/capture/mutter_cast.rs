//! GNOME recording: Mutter's own ScreenCast D-Bus API (what the portal and
//! gnome-remote-desktop use underneath) hands out a PipeWire node for a
//! monitor or an area, with no dialog. `capture::pipewire` reads it.

use super::LogicalRect;
use super::pipewire::{CropFn, Remote};
use crate::capture::VideoFrame;
use anyhow::{Context, Result};
use std::collections::HashMap;
use zbus::{OwnedObjectPath, Variant as Value};

const DEST: &str = "org.gnome.Mutter.ScreenCast";

#[derive(Clone)]
pub enum Target {
    /// A monitor, by connector name (the wl_output name).
    Monitor(String),
    /// An area of the desktop, in logical coordinates.
    Area(LogicalRect),
}

pub fn available(conn: &crate::dbus::Connection) -> bool {
    crate::dbus::name_has_owner(conn, DEST)
}

/// Record `target` until `sink` returns false.
pub fn stream(target: Target, cursor: bool, crop: CropFn, sink: Box<dyn FnMut(VideoFrame) -> bool>) -> Result<()> {
    let conn = crate::dbus::session().context("connecting to the session bus")?;
    let sc = crate::dbus::Proxy::new(&conn, DEST, "/org/gnome/Mutter/ScreenCast", "org.gnome.Mutter.ScreenCast")?;
    let session_path: OwnedObjectPath = sc.call("CreateSession", &(HashMap::<&str, Value>::new(),)).context("Mutter CreateSession")?;
    let session = crate::dbus::Proxy::new(&conn, DEST, session_path.as_str(), "org.gnome.Mutter.ScreenCast.Session")?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    // cursor-mode: 0 hidden, 1 embedded
    props.insert("cursor-mode", Value::from(cursor as u32));
    let stream_path: OwnedObjectPath = match &target {
        Target::Monitor(connector) => session.call("RecordMonitor", &(connector.as_str(), props)).context("Mutter RecordMonitor")?,
        &Target::Area((x, y, w, h)) => session
            .call("RecordArea", &(x.round() as i32, y.round() as i32, w.round().max(1.0) as i32, h.round().max(1.0) as i32, props))
            .context("Mutter RecordArea")?,
    };
    let stream = crate::dbus::Proxy::new(&conn, DEST, stream_path.as_str(), "org.gnome.Mutter.ScreenCast.Stream")?;
    // Subscribe before starting, or the node can be announced before we listen.
    let mut added = stream.receive_signal("PipeWireStreamAdded")?;
    session.call::<_, ()>("Start", &()).context("Mutter Start")?;
    let msg = added.next().context("Mutter didn't announce the PipeWire stream")?;
    let (node,): (u32,) = msg.body().deserialize()?;
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: Mutter screencast node {node}");
    }
    let r = super::pipewire::stream(Remote { fd: None, node }, crop, sink);
    let _ = session.call::<_, ()>("Stop", &());
    r
}
